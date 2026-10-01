//! The built-in recipe, [`Bm25Recipe`]: Lucene BM25 as a lateweave candidate
//! generator over every selected collection. It consumes only query text and
//! honours the subset.

use std::any::Any;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::SystemTime;

use lateweave::{
    Candidate, CandidateGenerator, DocumentKey, Gathered, Query, Requirements, Result,
    SearchPipeline, Subset,
};

use crate::analyzer::tokens;
use crate::recipe::{self, CollectionIndex, IndexDocument, Recipe};

pub const LEXICAL_K1: f32 = 1.5;
pub const LEXICAL_B: f32 = 0.75;

/// Lucene-variant BM25: `idf = ln(1 + (N - df + 0.5) / (df + 0.5))` times
/// `tf / (tf + k1 * (1 - b + b * dl / avgdl))`, summed over query terms
/// (repeated query terms count again). `N`, `df`, and `avgdl` come from
/// [`Statistics`], so several indexes can score as one corpus. Maintained
/// incrementally so a write tokenizes only the document it changes.
#[derive(Default)]
pub struct Bm25 {
    postings: HashMap<String, Vec<(u32, u32)>>,
    lengths: Vec<u32>,
    total_length: u64,
}

impl Bm25 {
    pub fn new<S: AsRef<str>>(texts: &[S]) -> Self {
        let mut index = Self::default();
        for text in texts {
            index.push(text.as_ref());
        }
        index
    }

    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    /// Appends a document under the next ID.
    pub fn push(&mut self, text: &str) {
        let document = self.lengths.len() as u32;
        let terms = tokens(text);
        self.lengths.push(terms.len() as u32);
        self.total_length += terms.len() as u64;
        let mut counts: HashMap<String, u32> = HashMap::new();
        for term in terms {
            *counts.entry(term).or_default() += 1;
        }
        for (term, count) in counts {
            self.postings
                .entry(term)
                .or_default()
                .push((document, count));
        }
    }

    /// Removes a document; later IDs shift down by one, matching the index.
    pub fn remove(&mut self, document: usize) {
        self.total_length -= u64::from(self.lengths.remove(document));
        let document = document as u32;
        self.postings.retain(|_, postings| {
            postings.retain_mut(|(id, _)| match (*id).cmp(&document) {
                Ordering::Less => true,
                Ordering::Equal => false,
                Ordering::Greater => {
                    *id -= 1;
                    true
                }
            });
            !postings.is_empty()
        });
    }

    /// Positive scores, best first, ties by ascending document ID. `eligible`
    /// masks documents by ID.
    fn search(
        &self,
        terms: &[String],
        limit: usize,
        eligible: Option<&[bool]>,
        statistics: &Statistics,
    ) -> Vec<(usize, f32)> {
        let documents = statistics.documents as f32;
        let average_length = statistics.total_length as f32 / documents;
        let mut scores = vec![0.0f32; self.lengths.len()];
        for term in terms {
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let frequency = statistics
                .frequencies
                .get(term)
                .copied()
                .unwrap_or_default() as f32;
            let idf = (1.0 + (documents - frequency + 0.5) / (frequency + 0.5)).ln();
            for &(document, term_count) in postings {
                if eligible.is_some_and(|mask| !mask[document as usize]) {
                    continue;
                }
                let tf = term_count as f32;
                let normalization = LEXICAL_K1
                    * (1.0 - LEXICAL_B
                        + LEXICAL_B * self.lengths[document as usize] as f32 / average_length);
                scores[document as usize] += idf * tf / (tf + normalization);
            }
        }
        let mut ranked: Vec<(usize, f32)> = scores
            .into_iter()
            .enumerate()
            .filter(|&(_, score)| score > 0.0)
            .collect();
        ranked
            .sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        ranked.truncate(limit);
        ranked
    }
}

/// Document count, total length, and query-term document frequencies, summed
/// over every index a search spans so their scores are comparable.
#[derive(Debug, Default)]
pub struct Statistics {
    documents: u64,
    total_length: u64,
    frequencies: HashMap<String, u64>,
}

impl Statistics {
    pub fn add(&mut self, index: &Bm25, terms: &[String]) {
        self.documents += index.len() as u64;
        self.total_length += index.total_length;
        for term in terms.iter().collect::<HashSet<_>>() {
            let frequency = index.postings.get(term).map_or(0, Vec::len) as u64;
            *self.frequencies.entry(term.clone()).or_default() += frequency;
        }
    }
}

/// One collection's BM25 index: `record_ids[i]` names document `i`.
#[derive(Default)]
pub struct Bm25Index {
    record_ids: Vec<Arc<str>>,
    positions: HashMap<Arc<str>, usize>,
    lexical: Bm25,
}

impl Bm25Index {
    fn push(&mut self, record_id: &str, text: &str) {
        let record_id: Arc<str> = record_id.into();
        self.positions
            .insert(record_id.clone(), self.record_ids.len());
        self.record_ids.push(record_id);
        self.lexical.push(text);
    }
}

impl CollectionIndex for Bm25Index {
    fn upsert(&mut self, record_id: &str, _: i64, text: &str) -> recipe::Result<()> {
        self.remove(record_id)?;
        self.push(record_id, text);
        Ok(())
    }

    fn remove(&mut self, record_id: &str) -> recipe::Result<()> {
        let Some(position) = self.positions.remove(record_id) else {
            return Ok(());
        };
        self.record_ids.remove(position);
        for later in &self.record_ids[position..] {
            *self.positions.get_mut(later).expect("indexed record") -= 1;
        }
        self.lexical.remove(position);
        Ok(())
    }
}

/// Lucene BM25 with statistics summed over every selected collection, so
/// scores compare; ties go to collection ID, then record ID. No reranker.
pub struct Bm25Recipe;

impl Recipe for Bm25Recipe {
    fn load(
        &self,
        _: &str,
        documents: Vec<IndexDocument>,
    ) -> recipe::Result<Box<dyn CollectionIndex>> {
        let mut index = Bm25Index::default();
        for document in documents {
            index.push(&document.record_id, &document.text);
        }
        Ok(Box::new(index))
    }

    fn pipeline<'a>(
        &self,
        query: &Query,
        indexes: &[(&'a str, &'a dyn CollectionIndex)],
    ) -> recipe::Result<SearchPipeline<'a>> {
        let terms = tokens(query.text());
        let mut statistics = Statistics::default();
        let mut corpora = Vec::with_capacity(indexes.len());
        for &(collection, index) in indexes {
            let index = (index as &dyn Any)
                .downcast_ref::<Bm25Index>()
                .ok_or("BM25 was given another recipe's index")?;
            statistics.add(&index.lexical, &terms);
            corpora.push(Corpus {
                id: collection,
                record_ids: &index.record_ids,
                index: &index.lexical,
            });
        }
        let gatherer = LexicalGatherer::new(corpora, statistics);
        Ok(SearchPipeline::new(Arc::new(gatherer), None))
    }
}

pub struct Corpus<'a> {
    pub id: &'a str,
    pub record_ids: &'a [Arc<str>],
    pub index: &'a Bm25,
}

/// BM25 over several corpora scored as one.
pub struct LexicalGatherer<'a> {
    corpora: Vec<Corpus<'a>>,
    requires: Requirements,
    statistics: Statistics,
}

impl<'a> LexicalGatherer<'a> {
    /// `statistics` must include every corpus and the query's terms.
    pub fn new(corpora: Vec<Corpus<'a>>, statistics: Statistics) -> Self {
        for corpus in &corpora {
            debug_assert_eq!(corpus.record_ids.len(), corpus.index.len());
        }
        Self {
            corpora,
            requires: Requirements::new(),
            statistics,
        }
    }
}

impl CandidateGenerator for LexicalGatherer<'_> {
    fn requires(&self) -> &Requirements {
        &self.requires
    }

    fn score_semantics(&self) -> &str {
        "bm25-lucene"
    }

    fn gather(&self, query: &Query, limit: usize, subset: Option<&Subset>) -> Result<Gathered> {
        let as_of = SystemTime::now();
        let terms = tokens(query.text());
        let mut scored: Vec<(f32, &Corpus, usize)> = Vec::new();
        for corpus in &self.corpora {
            let eligible = subset.map(|subset| {
                let restriction = subset.restriction(corpus.id);
                corpus
                    .record_ids
                    .iter()
                    .map(|id| restriction.is_some_and(|restriction| restriction.allows(id)))
                    .collect::<Vec<_>>()
            });
            scored.extend(
                corpus
                    .index
                    .search(&terms, limit, eligible.as_deref(), &self.statistics)
                    .into_iter()
                    .map(|(document, score)| (score, corpus, document)),
            );
        }
        scored.sort_unstable_by(|left, right| {
            right
                .0
                .total_cmp(&left.0)
                .then_with(|| left.1.id.cmp(right.1.id))
                .then_with(|| left.1.record_ids[left.2].cmp(&right.1.record_ids[right.2]))
        });
        scored.truncate(limit);
        let candidates = scored
            .into_iter()
            .enumerate()
            .map(|(rank, (score, corpus, document))| Candidate {
                key: DocumentKey::new(corpus.id, corpus.record_ids[document].clone()),
                gather_score: score,
                gather_rank: rank,
                provenance: "bm25".to_owned(),
            })
            .collect();
        Ok(Gathered { candidates, as_of })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXTS: [&str; 3] = [
        "CUDA illegal address after switching attention to bf16",
        "pytest fixture scope error when using async",
        "NCCL timeout: one worker never reached the barrier",
    ];

    fn corpus() -> (Bm25, Vec<Arc<str>>) {
        let ids = (0..TEXTS.len())
            .map(|i| Arc::from(format!("r{i}")))
            .collect();
        (Bm25::new(&TEXTS), ids)
    }

    fn gatherer<'a>(index: &'a Bm25, record_ids: &'a [Arc<str>]) -> LexicalGatherer<'a> {
        let statistics = own(
            index,
            &["cuda", "attention", "barrier", "worker", "timeout"],
        );
        LexicalGatherer::new(
            vec![Corpus {
                id: "t",
                record_ids,
                index,
            }],
            statistics,
        )
    }

    fn own(index: &Bm25, terms: &[&str]) -> Statistics {
        let mut statistics = Statistics::default();
        statistics.add(index, &strings(terms));
        statistics
    }

    fn strings(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|term| term.to_string()).collect()
    }

    fn ids(gathered: &Gathered) -> Vec<&str> {
        gathered
            .candidates
            .iter()
            .map(|candidate| {
                assert_eq!(candidate.key.corpus(), "t");
                candidate.key.id()
            })
            .collect()
    }

    #[test]
    fn lexical_ranks_matches_first_and_drops_zero_scores() {
        let (index, record_ids) = corpus();
        let gatherer = gatherer(&index, &record_ids);
        let found = gatherer
            .gather(&Query::new("worker barrier timeout"), 10, None)
            .unwrap();
        assert_eq!(ids(&found), ["r2"]);
        assert!(found.candidates[0].gather_score > 0.0);
        assert!(gatherer
            .gather(&Query::new(""), 10, None)
            .unwrap()
            .candidates
            .is_empty());
    }

    #[test]
    fn lexical_honours_the_subset() {
        let (index, record_ids) = corpus();
        let gatherer = gatherer(&index, &record_ids);
        let query = Query::new("cuda attention barrier");
        let subset = Subset::new().including("t", ["r1", "r2", "unknown"]);
        assert_eq!(
            ids(&gatherer.gather(&query, 10, Some(&subset)).unwrap()),
            ["r2"]
        );
        for subset in [
            Subset::new().including("t", [""; 0]),
            Subset::new().including("other", ["r0"]),
        ] {
            assert!(ids(&gatherer.gather(&query, 10, Some(&subset)).unwrap()).is_empty());
        }
    }

    #[test]
    fn lucene_scores_match_the_reference_formula() {
        let index = Bm25::new(&["a b", "a", "c c c d"]);
        let scores = index.search(&strings(&["a"]), 10, None, &own(&index, &["a"]));
        let average = 7.0 / 3.0;
        let idf = (1.0f32 + (3.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
        let expected = |length: f32| idf / (1.0 + 1.5 * (0.25 + 0.75 * length / average));
        assert_eq!(scores[0].0, 1);
        assert!((scores[0].1 - expected(1.0)).abs() < 1e-6);
        assert_eq!(scores[1].0, 0);
        assert!((scores[1].1 - expected(2.0)).abs() < 1e-6);
    }

    #[test]
    fn incremental_updates_match_a_fresh_build() {
        let mut index = Bm25::new(&["a b", "gone a", "c c c d"]);
        index.remove(1);
        index.push("a e");
        index.remove(0);
        index.push("b b a");
        let fresh = Bm25::new(&["c c c d", "a e", "b b a"]);
        for query in [["a"], ["b"], ["c"], ["e"], ["gone"]] {
            assert_eq!(
                index.search(&strings(&query), 10, None, &own(&index, &query)),
                fresh.search(&strings(&query), 10, None, &own(&fresh, &query))
            );
        }
        assert_eq!(index.len(), 3);
        assert!(!index.postings.contains_key("gone"));
    }

    #[test]
    fn shared_statistics_score_split_indexes_as_one_corpus() {
        let whole = Bm25::new(&["a b", "a", "c c c d", "a a d"]);
        let left = Bm25::new(&["a b", "a"]);
        let right = Bm25::new(&["c c c d", "a a d"]);
        let query = strings(&["a", "d", "a"]);
        let mut shared = Statistics::default();
        shared.add(&left, &query);
        shared.add(&right, &query);
        let expected = whole.search(&query, 10, None, &own(&whole, &["a", "d"]));
        let mut split: Vec<(usize, f32)> = left.search(&query, 10, None, &shared);
        split.extend(
            right
                .search(&query, 10, None, &shared)
                .into_iter()
                .map(|(document, score)| (document + 2, score)),
        );
        split.sort_unstable_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        assert_eq!(split, expected);
    }
}
