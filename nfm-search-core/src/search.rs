use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use frizbee::{Config as FrizbeeConfig, Matcher as FrizbeeMatcher, Scoring as FrizbeeScoring};
use rayon::prelude::*;

use crate::store::{ItemsSource, SearchPlan};
use crate::timing;

pub const DISPLAY_LIMIT: usize = 15;
pub const RESULT_LIMIT: usize = 1_000;
const SEARCH_TIMING_SAMPLE_RATE: usize = 256;
#[cfg(test)]
const SLAB_CAP: usize = 2_000_000;
#[cfg(test)]
const SCORE_MATCH: i32 = 16;
#[cfg(test)]
const SCORE_GAP_START: i32 = -3;
#[cfg(test)]
const SCORE_GAP_EXTENSION: i32 = -1;
#[cfg(test)]
const BOUNDARY_BONUS: i32 = SCORE_MATCH / 2;
#[cfg(test)]
const NON_WORD_BONUS: i32 = SCORE_MATCH / 2;
#[cfg(test)]
const CAMEL_CASE_BONUS: i32 = BOUNDARY_BONUS + SCORE_GAP_EXTENSION;
#[cfg(test)]
const BONUS_CONSECUTIVE: i32 = -(SCORE_GAP_START + SCORE_GAP_EXTENSION);
#[cfg(test)]
const BONUS_FIRST_CHAR_MULTIPLIER: i32 = 2;

fn frizbee_config() -> FrizbeeConfig {
    let mut scoring = FrizbeeScoring::default();
    scoring.gap_open_penalty = 3;
    scoring.capitalization_bonus = 7;
    scoring.delimiter_bonus = 8;
    scoring.matching_case_bonus = 0;
    scoring.consecutive_bonus = 4;
    scoring.first_match_boundary_multiplier = 2;
    FrizbeeConfig::default().max_typos(Some(0)).scoring(scoring)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SearchSortMode {
    #[default]
    Score,
    SourceOrder,
}

impl SearchSortMode {
    fn timing_label(self) -> &'static str {
        match self {
            Self::Score => "fuzzy",
            Self::SourceOrder => "fuzzy_source_order",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchOutput {
    pub results: Vec<SearchResult>,
    pub matched: usize,
    pub total: usize,
    pub(crate) match_bitmap: Option<MatchBitmap>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MatchBitmap {
    words: Vec<u64>,
    covered_len: usize,
}

impl MatchBitmap {
    fn from_indexes(covered_len: usize, indexes: &[u32]) -> Self {
        let mut words = vec![0; covered_len.div_ceil(64)];
        for &index in indexes {
            let index = index as usize;
            words[index / 64] |= 1u64 << (index % 64);
        }
        Self { words, covered_len }
    }

    pub(crate) fn merge(&mut self, other: &Self) {
        if other.covered_len > self.covered_len {
            self.words.resize(other.words.len(), 0);
            self.covered_len = other.covered_len;
        }
        for (target, source) in self.words.iter_mut().zip(&other.words) {
            *target |= source;
        }
    }

    fn matching_indexes(&self, end: usize) -> impl ParallelIterator<Item = usize> + '_ {
        self.words
            .par_iter()
            .enumerate()
            .flat_map_iter(move |(word_index, &remaining)| SetBitIndexes {
                base: word_index * 64,
                remaining,
                end,
            })
            .chain(self.covered_len.min(end)..end)
    }

    fn count_matches(&self, end: usize) -> usize {
        let covered = end.min(self.covered_len);
        let full_words = covered / 64;
        let mut count: usize = self.words[..full_words]
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        let trailing = covered % 64;
        if trailing != 0 {
            count += (self.words[full_words] & ((1u64 << trailing) - 1)).count_ones() as usize;
        }
        count + end.saturating_sub(self.covered_len)
    }
}

struct SetBitIndexes {
    base: usize,
    remaining: u64,
    end: usize,
}

impl Iterator for SetBitIndexes {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.remaining != 0 {
            let bit = self.remaining.trailing_zeros() as usize;
            self.remaining &= self.remaining - 1;
            let index = self.base + bit;
            if index < self.end {
                return Some(index);
            }
        }
        None
    }
}

#[cfg(test)]
mod match_bitmap_tests {
    use super::*;

    #[test]
    fn iterates_only_matches_and_the_uncovered_tail() {
        let bitmap = MatchBitmap::from_indexes(70, &[0, 2, 63, 64, 69]);
        let mut indexes: Vec<_> = bitmap.matching_indexes(73).collect();
        indexes.sort_unstable();
        assert_eq!(indexes, vec![0, 2, 63, 64, 69, 70, 71, 72]);
        assert_eq!(bitmap.count_matches(73), indexes.len());
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SearchResult {
    pub node_index: usize,
    pub score: u32,
    pub path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Candidate {
    node_index: usize,
    length: usize,
    score: u32,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then_with(|| other.length.cmp(&self.length))
            .then_with(|| other.node_index.cmp(&self.node_index))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorstFirst(Candidate);

impl Ord for WorstFirst {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl PartialOrd for WorstFirst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

enum CandidateCollector {
    Score(BinaryHeap<WorstFirst>),
    SourceOrder(Vec<Candidate>),
}

impl CandidateCollector {
    fn new(sort_mode: SearchSortMode) -> Self {
        match sort_mode {
            SearchSortMode::Score => Self::Score(BinaryHeap::new()),
            SearchSortMode::SourceOrder => Self::SourceOrder(Vec::new()),
        }
    }

    fn push(&mut self, candidate: Candidate) {
        match self {
            Self::Score(heap) => push_bounded(heap, candidate, RESULT_LIMIT),
            Self::SourceOrder(candidates) => {
                if candidates.len() < RESULT_LIMIT {
                    candidates.push(candidate);
                }
            }
        }
    }

    fn merge(&mut self, other: Self) {
        match (self, other) {
            (Self::Score(left), Self::Score(right)) => {
                for candidate in right.into_iter().map(|wrapped| wrapped.0) {
                    push_bounded(left, candidate, RESULT_LIMIT);
                }
            }
            (Self::SourceOrder(left), Self::SourceOrder(mut right)) => {
                left.append(&mut right);
                left.sort_unstable_by_key(|candidate| candidate.node_index);
                left.truncate(RESULT_LIMIT);
            }
            _ => unreachable!("candidate collectors must have the same sort mode"),
        }
    }

    fn into_sorted_candidates(self) -> Vec<Candidate> {
        match self {
            Self::Score(heap) => {
                let mut candidates: Vec<_> = heap.into_iter().map(|wrapped| wrapped.0).collect();
                candidates.sort_unstable_by(|left, right| right.cmp(left));
                candidates
            }
            Self::SourceOrder(mut candidates) => {
                candidates.sort_unstable_by_key(|candidate| candidate.node_index);
                candidates
            }
        }
    }
}

struct SearchAccumulator {
    candidates: CandidateCollector,
    matched: usize,
    path_buffer: Vec<u8>,
    stack_path_buffer: Box<[u8; 4096]>,
    utf8_count: usize,
    timing_samples: usize,
    path_us: u128,
    score_us: u128,
    retention_us: u128,
    matched_indexes: Option<Vec<u32>>,
    processed_since_fold_start: usize,
    cancelled: bool,
}

impl SearchAccumulator {
    fn new(sort_mode: SearchSortMode, collect_matches: bool) -> Self {
        Self {
            candidates: CandidateCollector::new(sort_mode),
            matched: 0,
            path_buffer: Vec::with_capacity(512),
            stack_path_buffer: Box::new([0u8; 4096]),
            utf8_count: 0,
            timing_samples: 0,
            path_us: 0,
            score_us: 0,
            retention_us: 0,
            matched_indexes: collect_matches.then(Vec::new),
            processed_since_fold_start: 0,
            cancelled: false,
        }
    }

    fn merge(&mut self, other: Self) {
        self.candidates.merge(other.candidates);
        self.matched += other.matched;
        self.utf8_count += other.utf8_count;
        self.timing_samples += other.timing_samples;
        self.path_us += other.path_us;
        self.score_us += other.score_us;
        self.retention_us += other.retention_us;
        if let (Some(left), Some(mut right)) = (&mut self.matched_indexes, other.matched_indexes) {
            left.append(&mut right);
        }
        self.cancelled |= other.cancelled;
    }
}

fn accumulate_frizbee<S, F, I>(
    snapshot: Arc<S>,
    indexes: I,
    query: &str,
    config: &FrizbeeConfig,
    sort_mode: SearchSortMode,
    is_cancelled: &F,
    collect_matches: bool,
    plan: &(dyn SearchPlan + '_),
    filters_items: bool,
) -> SearchAccumulator
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
    I: ParallelIterator<Item = usize>,
{
    indexes
        .fold(
            || {
                (
                    SearchAccumulator::new(sort_mode, collect_matches),
                    FrizbeeMatcher::from_query(query, config),
                )
            },
            |(mut state, mut matcher), node_index| {
                if state.cancelled {
                    return (state, matcher);
                }

                if (state.processed_since_fold_start & 0x3ff) == 0 && is_cancelled() {
                    state.cancelled = true;
                    return (state, matcher);
                }
                state.processed_since_fold_start += 1;

                if filters_items && !plan.includes(node_index) {
                    return (state, matcher);
                }

                let time_sample = (node_index & (SEARCH_TIMING_SAMPLE_RATE - 1)) == 0;
                let path_start = time_sample.then(Instant::now);
                let path_bytes = snapshot.get_string(
                    node_index,
                    &mut state.stack_path_buffer[..],
                    &mut state.path_buffer,
                );
                if let Some(path_start) = path_start {
                    state.path_us += timing::elapsed_us(path_start);
                    state.timing_samples += 1;
                }

                state.utf8_count += usize::from(!path_bytes.is_ascii());
                let score_start = time_sample.then(Instant::now);
                let score = std::str::from_utf8(path_bytes).ok().and_then(|path| {
                    matcher
                        .match_one(path, node_index as u32)
                        .map(|matched| matched.score as u32)
                });
                if let Some(score_start) = score_start {
                    state.score_us += timing::elapsed_us(score_start);
                }

                if let Some(score) = score {
                    state.matched += 1;
                    if let Some(indexes) = &mut state.matched_indexes {
                        indexes.push(node_index as u32);
                    }
                    let retention_start = time_sample.then(Instant::now);
                    state.candidates.push(Candidate {
                        node_index,
                        length: path_bytes.len(),
                        score,
                    });
                    if let Some(retention_start) = retention_start {
                        state.retention_us += timing::elapsed_us(retention_start);
                    }
                }

                (state, matcher)
            },
        )
        .map(|(state, _)| state)
        .reduce(
            || SearchAccumulator::new(sort_mode, collect_matches),
            |mut left, right| {
                left.merge(right);
                left
            },
        )
}

pub fn search<S, F>(snapshot: Arc<S>, query: &str, is_cancelled: F) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_with_sort(snapshot, query, SearchSortMode::Score, is_cancelled)
}

pub fn search_with_sort<S, F>(
    snapshot: Arc<S>,
    query: &str,
    sort_mode: SearchSortMode,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let end = snapshot.len();
    search_range_with_options(
        snapshot,
        query,
        0..end,
        sort_mode,
        is_cancelled,
        None,
        false,
    )
}

pub fn search_range<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_range_with_options(
        snapshot,
        query,
        range,
        SearchSortMode::Score,
        is_cancelled,
        None,
        false,
    )
}

pub fn search_range_with_sort<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    sort_mode: SearchSortMode,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_range_with_options(snapshot, query, range, sort_mode, is_cancelled, None, false)
}

pub(crate) fn search_range_with_match_collection<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    sort_mode: SearchSortMode,
    is_cancelled: F,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    search_range_with_options(snapshot, query, range, sort_mode, is_cancelled, None, true)
}

pub(crate) fn search_with_match_cache<S, F>(
    snapshot: Arc<S>,
    query: &str,
    sort_mode: SearchSortMode,
    is_cancelled: F,
    filter: Option<&MatchBitmap>,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let end = snapshot.len();
    search_range_with_options(
        snapshot,
        query,
        0..end,
        sort_mode,
        is_cancelled,
        filter,
        true,
    )
}

fn search_range_with_options<S, F>(
    snapshot: Arc<S>,
    query: &str,
    range: Range<usize>,
    sort_mode: SearchSortMode,
    is_cancelled: F,
    filter: Option<&MatchBitmap>,
    collect_matches: bool,
) -> Option<SearchOutput>
where
    S: ItemsSource + Send + Sync,
    F: Fn() -> bool + Sync,
{
    let total_start = Instant::now();
    let plan = snapshot.create_search_plan(query);
    let fuzzy_query = plan.fuzzy_query();
    let filters_items = plan.filters_items();
    let total = snapshot.len();
    let start_index = range.start.min(total);
    let end_index = range.end.min(total);
    let searched = end_index.saturating_sub(start_index);
    if is_cancelled() {
        return None;
    }

    if snapshot.is_empty() || searched == 0 {
        let total_us = timing::elapsed_us(total_start);
        timing::write(format!(
            "search_detail total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us=0 shown=0 matched=0 total={total}",
        ));
        return Some(SearchOutput {
            results: Vec::new(),
            matched: 0,
            total,
            match_bitmap: None,
        });
    }

    if fuzzy_query.is_empty() {
        let append_start = Instant::now();
        let mut included = Vec::with_capacity(RESULT_LIMIT.min(searched));
        let mut matched = 0;
        for index in start_index..end_index {
            if !filters_items || plan.includes(index) {
                matched += 1;
                if included.len() < RESULT_LIMIT {
                    included.push(index);
                }
            }
        }
        let mut output = SearchOutput {
            results: materialize_indexes(
                Arc::clone(&snapshot),
                included.into_iter(),
                RESULT_LIMIT,
                plan.as_ref(),
            ),
            matched,
            total,
            match_bitmap: None,
        };
        apply_custom_sort(&mut output.results, plan.as_ref());
        let append_us = timing::elapsed_us(append_start);
        let total_us = timing::elapsed_us(total_start);
        timing::write(format!(
            "search_detail mode=unfiltered total_us={total_us} parse_us=0 match_us=0 sort_us=0 append_us={append_us} shown={} matched={} total={}",
            output.results.len(),
            output.matched,
            output.total
        ));
        return Some(output);
    }

    let parse_start = Instant::now();
    let config = frizbee_config();
    let parse_us = timing::elapsed_us(parse_start);

    let match_start = Instant::now();
    debug_assert!(filter.is_none() || start_index == 0);
    let accumulator = if let Some(filter) = filter {
        accumulate_frizbee(
            Arc::clone(&snapshot),
            filter.matching_indexes(end_index),
            fuzzy_query,
            &config,
            sort_mode,
            &is_cancelled,
            collect_matches,
            plan.as_ref(),
            filters_items,
        )
    } else {
        accumulate_frizbee(
            Arc::clone(&snapshot),
            (start_index..end_index).into_par_iter(),
            fuzzy_query,
            &config,
            sort_mode,
            &is_cancelled,
            collect_matches,
            plan.as_ref(),
            filters_items,
        )
    };
    let match_us = timing::elapsed_us(match_start);
    let SearchAccumulator {
        candidates,
        matched,
        utf8_count,
        timing_samples,
        path_us,
        score_us,
        retention_us,
        cancelled,
        matched_indexes,
        ..
    } = accumulator;
    if cancelled || is_cancelled() {
        timing::write(format!(
            "search_cancelled mode={} match_us={match_us} matched={matched} total={total}",
            sort_mode.timing_label()
        ));
        return None;
    }

    let sort_start = Instant::now();
    let candidates = candidates.into_sorted_candidates();
    let sort_us = timing::elapsed_us(sort_start);

    let append_start = Instant::now();
    let mut path_buffer = Vec::with_capacity(512);
    let mut results: Vec<SearchResult> = candidates
        .into_iter()
        .map(|candidate| {
            let path = plan.display_text(candidate.node_index).unwrap_or_else(|| {
                snapshot.get_string_lossy(candidate.node_index, &mut path_buffer)
            });
            SearchResult {
                node_index: candidate.node_index,
                score: candidate.score,
                path,
            }
        })
        .collect();
    apply_custom_sort(&mut results, plan.as_ref());
    let append_us = timing::elapsed_us(append_start);
    let total_us = timing::elapsed_us(total_start);

    let sampled_scale = if timing_samples > 0 {
        SEARCH_TIMING_SAMPLE_RATE as u128
    } else {
        0
    };
    let utf8_path_estimate_us = path_us * sampled_scale;
    let ascii_score_estimate_us = score_us * sampled_scale;
    let retention_estimate_us = retention_us * sampled_scale;

    let searched = filter.map_or(searched, |filter| filter.count_matches(end_index));
    let match_bitmap = matched_indexes
        .as_deref()
        .map(|indexes| MatchBitmap::from_indexes(end_index, indexes));

    timing::write(format!(
        "search_detail mode={} total_us={total_us} parse_us={parse_us} match_us={match_us} sort_us={sort_us} append_us={append_us} utf8_path_estimate_us={utf8_path_estimate_us} ascii_score_estimate_us={ascii_score_estimate_us} char_fallback_estimate_us=0 retention_estimate_us={retention_estimate_us} timing_sample_rate={SEARCH_TIMING_SAMPLE_RATE} timing_samples={timing_samples} utf8_count={utf8_count} fallback_count=0 shown={} matched={matched} total={total} searched={searched}",
        sort_mode.timing_label(),
        results.len()
    ));

    Some(SearchOutput {
        results,
        matched,
        total,
        match_bitmap,
    })
}

/// Resolves the byte offsets used to highlight one already-ranked result.
///
/// Search results contain ranking data only, so callers can pay the
/// backtracking cost only for results they are about to display.
pub fn resolve_match_positions(query: &str, text: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut matcher = FrizbeeMatcher::from_query(query, &frizbee_config());
    let Some(mut matched) = matcher.match_one_indices(text, 0) else {
        return Vec::new();
    };
    matched.indices.reverse();
    let mut positions: Vec<_> = matched
        .indices
        .into_iter()
        .map(|index| {
            let mut index = index as usize;
            while index > 0 && !text.is_char_boundary(index) {
                index -= 1;
            }
            index
        })
        .collect();
    positions.dedup();
    positions
}

fn materialize_indexes<S, I>(
    snapshot: Arc<S>,
    indexes: I,
    limit: usize,
    plan: &(dyn SearchPlan + '_),
) -> Vec<SearchResult>
where
    S: ItemsSource + Send + Sync,
    I: Iterator<Item = usize>,
{
    let mut path_buffer = Vec::with_capacity(512);
    indexes
        .take(limit)
        .map(|node_index| SearchResult {
            node_index,
            score: 0,
            path: plan
                .display_text(node_index)
                .unwrap_or_else(|| snapshot.get_string_lossy(node_index, &mut path_buffer)),
        })
        .collect()
}

fn apply_custom_sort(results: &mut [SearchResult], plan: &(dyn SearchPlan + '_)) {
    if plan.has_custom_sort() {
        results.sort_by(|left, right| {
            plan.compare(left.node_index, right.node_index)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| left.node_index.cmp(&right.node_index))
        });
    }
}

fn push_bounded(heap: &mut BinaryHeap<WorstFirst>, candidate: Candidate, limit: usize) {
    if heap.len() < limit {
        heap.push(WorstFirst(candidate));
        return;
    }

    let Some(worst) = heap.peek() else {
        heap.push(WorstFirst(candidate));
        return;
    };

    if candidate > worst.0 {
        heap.pop();
        heap.push(WorstFirst(candidate));
    }
}

#[cfg(test)]
#[rustfmt::skip]
#[allow(dead_code)]
mod legacy_fzf {
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchKind {
    Fuzzy,
    Exact,
    Prefix,
    Suffix,
}

#[derive(Clone, Debug)]
struct Term {
    kind: MatchKind,
    inv: bool,
    case_sensitive: bool,
    text: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct TermSet {
    terms: Vec<Term>,
}

#[derive(Clone, Debug)]
struct AsciiPattern {
    term_sets: Vec<TermSet>,
    only_inv: bool,
}

impl AsciiPattern {
    fn parse(query: &str) -> Option<Self> {
        if !query.is_ascii() {
            return None;
        }

        let pattern = trim_suffix_spaces(query.trim_start());
        if pattern.is_empty() {
            return Some(Self {
                term_sets: Vec::new(),
                only_inv: false,
            });
        }

        let pattern_copy = pattern.replace("\\ ", "\t");
        let mut term_sets = Vec::new();
        let mut set = TermSet::default();
        let mut switch_set = false;
        let mut after_bar = false;

        for token in pattern_copy.split(' ') {
            let mut text = token.replace('\t', " ");
            if !after_bar && !set.terms.is_empty() && text == "|" {
                switch_set = false;
                after_bar = true;
                continue;
            }
            after_bar = false;

            if text.is_empty() {
                continue;
            }

            let lower_text = text.to_ascii_lowercase();
            let case_sensitive = text != lower_text;
            if !case_sensitive {
                text = lower_text;
            }

            let mut kind = MatchKind::Fuzzy;
            let mut inv = false;

            if let Some(rest) = text.strip_prefix('!') {
                inv = true;
                kind = MatchKind::Exact;
                text = rest.to_string();
            }

            if text.ends_with('$') && text != "$" {
                kind = MatchKind::Suffix;
                text.pop();
            }

            if let Some(rest) = text.strip_prefix('\'') {
                if !inv {
                    kind = MatchKind::Exact;
                } else {
                    kind = MatchKind::Fuzzy;
                }
                text = rest.to_string();
            } else if let Some(rest) = text.strip_prefix('^') {
                kind = MatchKind::Prefix;
                text = rest.to_string();
            }

            if text.is_empty() {
                continue;
            }

            if switch_set {
                term_sets.push(set);
                set = TermSet::default();
            }

            set.terms.push(Term {
                kind,
                inv,
                case_sensitive,
                text: text.into_bytes(),
            });
            switch_set = true;
        }

        if !set.terms.is_empty() {
            term_sets.push(set);
        }

        let only_inv = !term_sets.is_empty()
            && term_sets
                .iter()
                .all(|set| set.terms.len() == 1 && set.terms[0].inv);

        Some(Self {
            term_sets,
            only_inv,
        })
    }

    fn score(&self, text: &[u8], scratch: &mut MatchScratch) -> Option<u32> {
        if self.term_sets.is_empty() {
            return Some(1);
        }

        if self.term_sets.len() == 1 && self.term_sets[0].terms.len() == 1 {
            let term = &self.term_sets[0].terms[0];
            let res = match_ascii(term, text, scratch);
            if term.inv {
                return if res.start >= 0 { None } else { Some(1) };
            }

            return if res.start >= 0 && res.score > 0 {
                Some(res.score as u32)
            } else {
                None
            };
        }

        if self.only_inv {
            let mut final_score = 0;
            for term_set in &self.term_sets {
                let term = &term_set.terms[0];
                let res = match_ascii(term, text, scratch);
                final_score += res.score;
            }
            return if final_score > 0 { None } else { Some(1) };
        }

        let mut total_score = 0;
        for term_set in &self.term_sets {
            let mut current_score = 0;
            let mut matched = false;
            for term in &term_set.terms {
                let res = match_ascii(term, text, scratch);
                if res.start >= 0 {
                    if term.inv {
                        continue;
                    }

                    current_score = res.score;
                    matched = true;
                    break;
                }

                if term.inv {
                    current_score = 0;
                    matched = true;
                }
            }

            if matched {
                total_score += current_score;
            } else {
                return None;
            }
        }

        if total_score > 0 {
            Some(total_score as u32)
        } else {
            None
        }
    }

    fn positions(&self, text: &[u8], scratch: &mut MatchScratch) -> Vec<usize> {
        if !text.is_ascii() {
            return Vec::new();
        }

        let mut positions = Vec::new();
        for term_set in &self.term_sets {
            for term in &term_set.terms {
                if term.inv {
                    continue;
                }

                if append_term_positions(term, text, scratch, &mut positions) {
                    break;
                }
            }
        }

        positions.sort_unstable();
        positions.dedup();
        positions
    }
}

#[derive(Default)]
struct MatchScratch {
    text_copy: Vec<u8>,
    initial_scores: Vec<i32>,
    consecutive_scores: Vec<i32>,
    bonuses: Vec<i32>,
    first_occurrence: Vec<usize>,
    score_matrix: Vec<i32>,
    consecutive_matrix: Vec<i32>,
}

#[derive(Clone, Copy)]
struct FzfResult {
    start: isize,
    end: isize,
    score: i32,
}

fn match_ascii(term: &Term, text: &[u8], scratch: &mut MatchScratch) -> FzfResult {
    match term.kind {
        MatchKind::Fuzzy => {
            fzf_fuzzy_match_v2_ascii(term.case_sensitive, text, &term.text, scratch, None)
        }
        MatchKind::Exact => fzf_exact_match_naive_ascii(term.case_sensitive, text, &term.text),
        MatchKind::Prefix => fzf_prefix_match_ascii(term.case_sensitive, text, &term.text),
        MatchKind::Suffix => fzf_suffix_match_ascii(term.case_sensitive, text, &term.text),
    }
}

fn append_term_positions(
    term: &Term,
    text: &[u8],
    scratch: &mut MatchScratch,
    positions: &mut Vec<usize>,
) -> bool {
    let res = match term.kind {
        MatchKind::Fuzzy => {
            return fzf_fuzzy_match_v2_ascii(
                term.case_sensitive,
                text,
                &term.text,
                scratch,
                Some(positions),
            )
            .start
                >= 0;
        }
        _ => match_ascii(term, text, scratch),
    };

    if res.start < 0 || res.end < res.start {
        return false;
    }

    append_positions_ascii(
        term.case_sensitive,
        text,
        &term.text,
        res.start as usize,
        res.end as usize,
        positions,
    );
    true
}

fn fzf_prefix_match_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return no_match();
    }

    let trimmed_len = if !is_ascii_whitespace(pattern[0]) {
        leading_whitespace_ascii(text)
    } else {
        0
    };

    if text.len().saturating_sub(trimmed_len) < pattern.len() {
        return no_match();
    }

    for index in 0..pattern.len() {
        if eq_byte(text[trimmed_len + index], pattern[index], case_sensitive) {
            continue;
        }
        return no_match();
    }

    let start = trimmed_len;
    let end = trimmed_len + pattern.len();
    FzfResult {
        start: start as isize,
        end: end as isize,
        score: calculate_score_ascii(case_sensitive, text, pattern, start, end),
    }
}

fn fzf_suffix_match_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return no_match();
    }

    let trimmed_len = if !is_ascii_whitespace(*pattern.last().expect("non-empty")) {
        trim_trailing_spaces_ascii(text)
    } else {
        text.len()
    };

    if trimmed_len < pattern.len() {
        return no_match();
    }

    let start = trimmed_len - pattern.len();
    for index in 0..pattern.len() {
        if eq_byte(text[start + index], pattern[index], case_sensitive) {
            continue;
        }
        return no_match();
    }

    FzfResult {
        start: start as isize,
        end: trimmed_len as isize,
        score: calculate_score_ascii(
            case_sensitive,
            &text[..trimmed_len],
            pattern,
            start,
            trimmed_len,
        ),
    }
}

fn fzf_exact_match_naive_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    if text.len() < pattern.len() || fuzzy_index_of_ascii(text, pattern, case_sensitive).is_none() {
        return no_match();
    }

    let mut pattern_index = 0usize;
    let mut best_pos: Option<usize> = None;
    let mut bonus = 0;
    let mut best_bonus = -1;
    let mut index = 0usize;
    while index < text.len() {
        if eq_byte(text[index], pattern[pattern_index], case_sensitive) {
            if pattern_index == 0 {
                bonus = bonus_at_ascii(text, index);
            }

            pattern_index += 1;
            if pattern_index == pattern.len() {
                if bonus > best_bonus {
                    best_pos = Some(index);
                    best_bonus = bonus;
                }

                if bonus == BOUNDARY_BONUS {
                    break;
                }

                index = index.saturating_sub(pattern_index - 1);
                pattern_index = 0;
                bonus = 0;
            }
        } else {
            index = index.saturating_sub(pattern_index);
            pattern_index = 0;
            bonus = 0;
        }
        index += 1;
    }

    let Some(best_pos) = best_pos else {
        return no_match();
    };
    let start = best_pos + 1 - pattern.len();
    let end = best_pos + 1;
    FzfResult {
        start: start as isize,
        end: end as isize,
        score: calculate_score_ascii(case_sensitive, text, pattern, start, end),
    }
}

fn fzf_fuzzy_match_v2_ascii(
    case_sensitive: bool,
    text: &[u8],
    pattern: &[u8],
    scratch: &mut MatchScratch,
    mut positions: Option<&mut Vec<usize>>,
) -> FzfResult {
    let pattern_size = pattern.len();
    let text_size = text.len();

    if pattern_size == 0 {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    if pattern_size.saturating_mul(text_size) >= SLAB_CAP {
        let res = fuzzy_match_v1_ascii(case_sensitive, text, pattern);
        if res.start >= 0 {
            if let Some(positions) = positions.as_deref_mut() {
                append_positions_ascii(
                    case_sensitive,
                    text,
                    pattern,
                    res.start as usize,
                    res.end as usize,
                    positions,
                );
            }
        }
        return res;
    }

    let Some(first_index_of) = fuzzy_index_of_ascii(text, pattern, case_sensitive) else {
        return no_match();
    };

    scratch.text_copy.clear();
    scratch.text_copy.extend_from_slice(text);
    scratch.initial_scores.clear();
    scratch.initial_scores.resize(text_size, 0);
    scratch.consecutive_scores.clear();
    scratch.consecutive_scores.resize(text_size, 0);
    scratch.bonuses.clear();
    scratch.bonuses.resize(text_size, 0);
    scratch.first_occurrence.clear();
    scratch.first_occurrence.resize(pattern_size, 0);

    let mut max_score = 0;
    let mut max_score_pos = 0usize;
    let mut pattern_index = 0usize;
    let mut last_index = 0usize;
    let first_pattern_char = normalize_byte(pattern[0], case_sensitive);
    let mut current_pattern_char = first_pattern_char;
    let mut previous_initial_score = 0;
    let mut previous_class = CharClass::NonWord;
    let mut in_gap = false;

    for i in first_index_of..text_size {
        let mut current_char = scratch.text_copy[i];
        let current_class = class_of_ascii(current_char);
        if !case_sensitive && current_class == CharClass::CharUpper {
            current_char = to_lower_ascii(current_char);
        }

        scratch.text_copy[i] = current_char;
        let bonus = calculate_bonus(previous_class, current_class);
        scratch.bonuses[i] = bonus;
        previous_class = current_class;

        if current_char == current_pattern_char {
            if pattern_index < pattern_size {
                scratch.first_occurrence[pattern_index] = i;
                pattern_index += 1;
                current_pattern_char =
                    normalize_byte(pattern[pattern_index.min(pattern_size - 1)], case_sensitive);
            }

            last_index = i;
        }

        if current_char == first_pattern_char {
            let score = SCORE_MATCH + bonus * BONUS_FIRST_CHAR_MULTIPLIER;
            scratch.initial_scores[i] = score;
            scratch.consecutive_scores[i] = 1;
            if pattern_size == 1 && score > max_score {
                max_score = score;
                max_score_pos = i;
                if bonus == BOUNDARY_BONUS {
                    break;
                }
            }

            in_gap = false;
        } else {
            scratch.initial_scores[i] = if in_gap {
                (previous_initial_score + SCORE_GAP_EXTENSION).max(0)
            } else {
                (previous_initial_score + SCORE_GAP_START).max(0)
            };
            scratch.consecutive_scores[i] = 0;
            in_gap = true;
        }

        previous_initial_score = scratch.initial_scores[i];
    }

    if pattern_index != pattern_size {
        return no_match();
    }

    if pattern_size == 1 {
        if let Some(positions) = positions.as_deref_mut() {
            positions.push(max_score_pos);
        }
        return FzfResult {
            start: max_score_pos as isize,
            end: max_score_pos as isize + 1,
            score: max_score,
        };
    }

    let first_occurrence_of_first_char = scratch.first_occurrence[0];
    let width = last_index - first_occurrence_of_first_char + 1;
    scratch.score_matrix.clear();
    scratch.score_matrix.resize(width * pattern_size, 0);
    scratch.consecutive_matrix.clear();
    scratch.consecutive_matrix.resize(width * pattern_size, 0);

    for index in 0..width {
        let source = first_occurrence_of_first_char + index;
        scratch.score_matrix[index] = scratch.initial_scores[source];
        scratch.consecutive_matrix[index] = scratch.consecutive_scores[source];
    }

    for off in 0..(pattern_size - 1) {
        let pattern_char_offset = scratch.first_occurrence[off + 1];
        let current_pattern_char = normalize_byte(pattern[off + 1], case_sensitive);
        let pattern_index = off + 1;
        let row = pattern_index * width;
        let mut in_gap = false;

        for column in pattern_char_offset..=last_index {
            let local = column - first_occurrence_of_first_char;
            let matrix_index = row + local;
            let left_score = if local == 0 {
                0
            } else {
                scratch.score_matrix[matrix_index - 1]
            };
            let score = if in_gap {
                left_score + SCORE_GAP_EXTENSION
            } else {
                left_score + SCORE_GAP_START
            };

            let mut diagonal_score = 0;
            let mut consecutive = 0;
            if scratch.text_copy[column] == current_pattern_char && local > 0 {
                let diagonal_index = matrix_index - 1 - width;
                diagonal_score = scratch.score_matrix[diagonal_index] + SCORE_MATCH;
                let mut bonus = scratch.bonuses[column];
                consecutive = scratch.consecutive_matrix[diagonal_index] + 1;
                if bonus == BOUNDARY_BONUS {
                    consecutive = 1;
                } else if consecutive > 1 {
                    let consecutive_start = column + 1 - consecutive as usize;
                    bonus = bonus
                        .max(BONUS_CONSECUTIVE)
                        .max(scratch.bonuses[consecutive_start]);
                }

                if diagonal_score + bonus < score {
                    diagonal_score += scratch.bonuses[column];
                    consecutive = 0;
                } else {
                    diagonal_score += bonus;
                }
            }

            scratch.consecutive_matrix[matrix_index] = consecutive;
            in_gap = diagonal_score < score;
            let score2 = 0.max(diagonal_score.max(score));
            if pattern_index == pattern_size - 1 && score2 > max_score {
                max_score = score2;
                max_score_pos = column;
            }

            scratch.score_matrix[matrix_index] = score2;
        }
    }

    let mut start = max_score_pos;
    let mut pattern_index = pattern_size - 1;
    let mut prefer_match = true;

    loop {
        let row = pattern_index * width;
        let column = start - first_occurrence_of_first_char;
        let current_score = scratch.score_matrix[row + column];

        let mut diagonal_score = 0;
        if pattern_index > 0 && start >= scratch.first_occurrence[pattern_index] {
            diagonal_score = scratch.score_matrix[row - width + column - 1];
        }

        let mut left_score = 0;
        if start > scratch.first_occurrence[pattern_index] {
            left_score = scratch.score_matrix[row + column - 1];
        }

        if current_score > diagonal_score
            && (current_score > left_score || (current_score == left_score && prefer_match))
        {
            if let Some(positions) = positions.as_deref_mut() {
                positions.push(start);
            }

            if pattern_index == 0 {
                break;
            }

            pattern_index -= 1;
        }

        if start == first_occurrence_of_first_char {
            break;
        }

        prefer_match = scratch.consecutive_matrix[row + column] > 1
            || (row + width + column + 1 < scratch.consecutive_matrix.len()
                && scratch.consecutive_matrix[row + width + column + 1] > 0);
        start -= 1;
    }

    FzfResult {
        start: start as isize,
        end: max_score_pos as isize + 1,
        score: max_score,
    }
}

fn fuzzy_match_v1_ascii(case_sensitive: bool, text: &[u8], pattern: &[u8]) -> FzfResult {
    if pattern.is_empty() {
        return FzfResult {
            start: 0,
            end: 0,
            score: 0,
        };
    }

    if fuzzy_index_of_ascii(text, pattern, case_sensitive).is_none() {
        return no_match();
    }

    let mut pattern_index = 0usize;
    let mut start_index: Option<usize> = None;
    let mut end_index: Option<usize> = None;

    for (index, &current_char) in text.iter().enumerate() {
        if eq_byte(current_char, pattern[pattern_index], case_sensitive) {
            if start_index.is_none() {
                start_index = Some(index);
            }

            pattern_index += 1;
            if pattern_index == pattern.len() {
                end_index = Some(index + 1);
                break;
            }
        }
    }

    let (Some(mut start), Some(end)) = (start_index, end_index) else {
        return no_match();
    };

    pattern_index -= 1;
    for index in (start..end).rev() {
        if eq_byte(text[index], pattern[pattern_index], case_sensitive) {
            if pattern_index == 0 {
                start = index;
                break;
            }
            pattern_index -= 1;
        }
    }

    FzfResult {
        start: start as isize,
        end: end as isize,
        score: calculate_score_ascii(case_sensitive, text, pattern, start, end),
    }
}

fn append_positions_ascii(
    case_sensitive: bool,
    text: &[u8],
    pattern: &[u8],
    start_index: usize,
    end_index: usize,
    positions: &mut Vec<usize>,
) {
    let mut pattern_index = 0usize;
    for index in start_index..end_index {
        if pattern_index >= pattern.len() {
            break;
        }

        if eq_byte(text[index], pattern[pattern_index], case_sensitive) {
            positions.push(index);
            pattern_index += 1;
        }
    }
}

fn calculate_score_ascii(
    case_sensitive: bool,
    text: &[u8],
    pattern: &[u8],
    start_index: usize,
    end_index: usize,
) -> i32 {
    let mut pattern_index = 0usize;
    let mut score = 0;
    let mut consecutive = 0;
    let mut in_gap = false;
    let mut first_bonus = 0;
    let mut prev_class = if start_index > 0 {
        class_of_ascii(text[start_index - 1])
    } else {
        CharClass::NonWord
    };

    for &raw_char in &text[start_index..end_index] {
        let current_class = class_of_ascii(raw_char);
        let current_char = normalize_byte(raw_char, case_sensitive);
        let pattern_char = normalize_byte(pattern[pattern_index], case_sensitive);

        if current_char == pattern_char {
            score += SCORE_MATCH;
            let mut bonus = calculate_bonus(prev_class, current_class);
            if consecutive == 0 {
                first_bonus = bonus;
            } else {
                if bonus == BOUNDARY_BONUS {
                    first_bonus = bonus;
                }
                bonus = bonus.max(first_bonus).max(BONUS_CONSECUTIVE);
            }

            if pattern_index == 0 {
                score += bonus * BONUS_FIRST_CHAR_MULTIPLIER;
            } else {
                score += bonus;
            }

            in_gap = false;
            consecutive += 1;
            pattern_index += 1;
        } else {
            score += if in_gap {
                SCORE_GAP_EXTENSION
            } else {
                SCORE_GAP_START
            };
            in_gap = true;
            consecutive = 0;
            first_bonus = 0;
        }

        prev_class = current_class;
    }

    score
}

fn fuzzy_index_of_ascii(input: &[u8], pattern: &[u8], case_sensitive: bool) -> Option<usize> {
    let mut index = 0usize;
    let mut first_index = 0usize;

    for (pattern_index, &byte) in pattern.iter().enumerate() {
        let found = index_of_byte_ascii(input, byte, index, case_sensitive)?;
        if pattern_index == 0 && found > 0 {
            first_index = found - 1;
        }
        index = found + 1;
    }

    Some(first_index)
}

fn index_of_byte_ascii(
    input: &[u8],
    value: u8,
    start_index: usize,
    case_sensitive: bool,
) -> Option<usize> {
    let value = normalize_byte(value, case_sensitive);
    for (index, &current) in input.iter().enumerate().skip(start_index) {
        if normalize_byte(current, case_sensitive) == value {
            return Some(index);
        }
    }
    None
}

fn bonus_at_ascii(text: &[u8], index: usize) -> i32 {
    let current = class_of_ascii(text[index]);
    let previous = if index > 0 {
        class_of_ascii(text[index - 1])
    } else {
        CharClass::NonWord
    };
    calculate_bonus(previous, current)
}

fn calculate_bonus(previous: CharClass, current: CharClass) -> i32 {
    if previous == CharClass::NonWord && current != CharClass::NonWord {
        return BOUNDARY_BONUS;
    }

    if (previous == CharClass::CharLower && current == CharClass::CharUpper)
        || (previous != CharClass::Digit && current == CharClass::Digit)
    {
        return CAMEL_CASE_BONUS;
    }

    if current == CharClass::NonWord {
        return NON_WORD_BONUS;
    }

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CharClass {
    NonWord,
    CharLower,
    CharUpper,
    Digit,
}

fn class_of_ascii(byte: u8) -> CharClass {
    match byte {
        b'a'..=b'z' => CharClass::CharLower,
        b'A'..=b'Z' => CharClass::CharUpper,
        b'0'..=b'9' => CharClass::Digit,
        _ => CharClass::NonWord,
    }
}

fn normalize_byte(byte: u8, case_sensitive: bool) -> u8 {
    if case_sensitive {
        byte
    } else {
        to_lower_ascii(byte)
    }
}

fn eq_byte(left: u8, right: u8, case_sensitive: bool) -> bool {
    normalize_byte(left, case_sensitive) == normalize_byte(right, case_sensitive)
}

fn to_lower_ascii(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte + 32
    } else {
        byte
    }
}

fn is_ascii_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn leading_whitespace_ascii(text: &[u8]) -> usize {
    text.iter()
        .position(|byte| !is_ascii_whitespace(*byte))
        .unwrap_or(text.len())
}

fn trim_trailing_spaces_ascii(text: &[u8]) -> usize {
    text.iter()
        .rposition(|byte| !is_ascii_whitespace(*byte))
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn trim_suffix_spaces(value: &str) -> &str {
    value.trim_end_matches(' ')
}

fn no_match() -> FzfResult {
    FzfResult {
        start: -1,
        end: -1,
        score: 0,
    }
}

#[cfg(test)]
mod search_sort_tests {
    use super::*;
    use crate::store::FlatSnapshot;

    #[test]
    fn source_order_returns_first_matches_up_to_result_limit() {
        let item_count = RESULT_LIMIT + 25;
        let snapshot = Arc::new(FlatSnapshot::from_items(
            (0..item_count).map(|index| (format!("match-{index:04}"), ())),
        ));

        let output = search_with_sort(snapshot, "match", SearchSortMode::SourceOrder, || false)
            .expect("search should complete");

        assert_eq!(output.matched, item_count);
        assert_eq!(output.results.len(), RESULT_LIMIT);
        assert_eq!(
            output
                .results
                .iter()
                .map(|result| result.node_index)
                .collect::<Vec<_>>(),
            (0..RESULT_LIMIT).collect::<Vec<_>>()
        );
    }

    #[test]
    fn sort_modes_report_the_same_match_count() {
        let snapshot = Arc::new(FlatSnapshot::from_items([
            ("prefix needle", ()),
            ("not a result", ()),
            ("needle", ()),
            ("another needle result", ()),
        ]));

        let score = search_with_sort(
            Arc::clone(&snapshot),
            "needle",
            SearchSortMode::Score,
            || false,
        )
        .expect("score search should complete");
        let source_order =
            search_with_sort(snapshot, "needle", SearchSortMode::SourceOrder, || false)
                .expect("source-order search should complete");

        assert_eq!(source_order.matched, score.matched);
        assert_eq!(
            source_order
                .results
                .iter()
                .map(|result| result.node_index)
                .collect::<Vec<_>>(),
            vec![0, 2, 3]
        );
    }

    #[test]
    fn search_defers_positions_until_the_result_is_materialized() {
        let snapshot = Arc::new(FlatSnapshot::from_items([("alpha-beta", ())]));
        let output = search(snapshot, "ab", || false).expect("search should complete");

        assert_eq!(output.results[0].path, "alpha-beta");
        assert_eq!(resolve_match_positions("ab", "alpha-beta"), vec![0, 6]);
    }
}

#[test]
fn fuzzy_match_consecutive_at_start_does_not_underflow() {
    let mut scratch = MatchScratch::default();

    let result = fzf_fuzzy_match_v2_ascii(false, b"rights", b"ri", &mut scratch, None);

    assert!(result.score > 0);
    assert_eq!(result.start, 0);
    assert_eq!(result.end, 2);
}

#[cfg(test)]
mod tests_from_fzf {
    use super::*;

    #[derive(Clone, Copy)]
    enum Algorithm {
        FuzzyV1,
        FuzzyV2,
        Exact,
        Prefix,
        Suffix,
    }

    fn run_match(
        algorithm: Algorithm,
        case_sensitive: bool,
        text: &[u8],
        pattern: &[u8],
    ) -> FzfResult {
        let mut scratch = MatchScratch::default();
        match algorithm {
            Algorithm::FuzzyV1 => fuzzy_match_v1_ascii(case_sensitive, text, pattern),
            Algorithm::FuzzyV2 => {
                fzf_fuzzy_match_v2_ascii(case_sensitive, text, pattern, &mut scratch, None)
            }
            Algorithm::Exact => fzf_exact_match_naive_ascii(case_sensitive, text, pattern),
            Algorithm::Prefix => fzf_prefix_match_ascii(case_sensitive, text, pattern),
            Algorithm::Suffix => fzf_suffix_match_ascii(case_sensitive, text, pattern),
        }
    }

    fn assert_score(
        algorithm: Algorithm,
        case_sensitive: bool,
        text: &[u8],
        pattern: &[u8],
        expected_score: i32,
    ) {
        let result = run_match(algorithm, case_sensitive, text, pattern);
        assert_eq!(
            result.score,
            expected_score,
            "pattern: {:?}, text: {:?}, case_sensitive: {case_sensitive}",
            String::from_utf8_lossy(pattern),
            String::from_utf8_lossy(text)
        );
    }

    #[test]
    fn fuzzy_match_scores_match_fzf_cases() {
        for algorithm in [Algorithm::FuzzyV1, Algorithm::FuzzyV2] {
            assert_score(
                algorithm,
                false,
                b"fooBarbaz1",
                b"obz",
                SCORE_MATCH * 3 + CAMEL_CASE_BONUS + SCORE_GAP_START + SCORE_GAP_EXTENSION * 3,
            );
            assert_score(
                algorithm,
                false,
                b"foo bar baz",
                b"fbb",
                SCORE_MATCH * 3
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + BOUNDARY_BONUS * 2
                    + 2 * SCORE_GAP_START
                    + 4 * SCORE_GAP_EXTENSION,
            );
            assert_score(
                algorithm,
                false,
                b"/AutomatorDocument.icns",
                b"rdoc",
                SCORE_MATCH * 4 + CAMEL_CASE_BONUS + BONUS_CONSECUTIVE * 2,
            );
            assert_score(
                algorithm,
                false,
                b"/man1/zshcompctl.1",
                b"zshc",
                SCORE_MATCH * 4 + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER + BOUNDARY_BONUS * 3,
            );
            assert_score(
                algorithm,
                false,
                b"/.oh-my-zsh/cache",
                b"zshc",
                SCORE_MATCH * 4
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + BOUNDARY_BONUS * 2
                    + SCORE_GAP_START
                    + BOUNDARY_BONUS,
            );
            assert_score(
                algorithm,
                false,
                b"ab0123 456",
                b"12356",
                SCORE_MATCH * 5 + BONUS_CONSECUTIVE * 3 + SCORE_GAP_START + SCORE_GAP_EXTENSION,
            );
            assert_score(
                algorithm,
                false,
                b"foo/bar/baz",
                b"fbb",
                SCORE_MATCH * 3
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + BOUNDARY_BONUS * 2
                    + 2 * SCORE_GAP_START
                    + 4 * SCORE_GAP_EXTENSION,
            );
            assert_score(
                algorithm,
                true,
                b"FooBarBaz",
                b"FBB",
                SCORE_MATCH * 3
                    + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                    + CAMEL_CASE_BONUS * 2
                    + SCORE_GAP_START * 2
                    + SCORE_GAP_EXTENSION * 2,
            );
            assert_score(algorithm, true, b"fooBarbaz", b"oBZ", 0);
            assert_score(algorithm, true, b"Foo Bar Baz", b"fbb", 0);
            assert_score(algorithm, true, b"fooBarbaz", b"fooBarbazz", 0);
        }
    }

    #[test]
    fn fuzzy_match_v1_forward_case_score_matches_fzf() {
        assert_score(
            Algorithm::FuzzyV1,
            false,
            b"foobar fb",
            b"fb",
            SCORE_MATCH * 2
                + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER
                + SCORE_GAP_START
                + SCORE_GAP_EXTENSION,
        );
    }

    #[test]
    fn exact_match_naive_scores_match_fzf_cases() {
        assert_score(Algorithm::Exact, true, b"fooBarbaz", b"oBA", 0);
        assert_score(Algorithm::Exact, true, b"fooBarbaz", b"fooBarbazz", 0);
        assert_score(
            Algorithm::Exact,
            false,
            b"fooBarbaz",
            b"oba",
            SCORE_MATCH * 3 + CAMEL_CASE_BONUS + BONUS_CONSECUTIVE,
        );
        assert_score(
            Algorithm::Exact,
            false,
            b"/AutomatorDocument.icns",
            b"rdoc",
            SCORE_MATCH * 4 + CAMEL_CASE_BONUS + BONUS_CONSECUTIVE * 2,
        );
        assert_score(
            Algorithm::Exact,
            false,
            b"/man1/zshcompctl.1",
            b"zshc",
            SCORE_MATCH * 4 + BOUNDARY_BONUS * (BONUS_FIRST_CHAR_MULTIPLIER + 3),
        );
        assert_score(
            Algorithm::Exact,
            false,
            b"/.oh-my-zsh/cache",
            b"zsh/c",
            SCORE_MATCH * 5 + BOUNDARY_BONUS * (BONUS_FIRST_CHAR_MULTIPLIER + 3) + BOUNDARY_BONUS,
        );
    }

    #[test]
    fn exact_match_naive_backward_case_score_matches_fzf() {
        assert_score(
            Algorithm::Exact,
            false,
            b"foobar foob",
            b"oo",
            SCORE_MATCH * 2 + BONUS_CONSECUTIVE,
        );
    }

    #[test]
    fn prefix_match_scores_match_fzf_cases() {
        let score =
            SCORE_MATCH * 3 + BOUNDARY_BONUS * BONUS_FIRST_CHAR_MULTIPLIER + BOUNDARY_BONUS * 2;

        assert_score(Algorithm::Prefix, true, b"fooBarbaz", b"Foo", 0);
        assert_score(Algorithm::Prefix, false, b"fooBarBaz", b"baz", 0);
        assert_score(Algorithm::Prefix, false, b"fooBarbaz", b"foo", score);
        assert_score(Algorithm::Prefix, false, b"foOBarBaZ", b"foo", score);
        assert_score(Algorithm::Prefix, false, b"f-oBarbaz", b"f-o", score);
        assert_score(Algorithm::Prefix, false, b" fooBar", b"foo", score);
        assert_score(Algorithm::Prefix, false, b" fooBar", b" fo", score);
        assert_score(Algorithm::Prefix, false, b"     fo", b"foo", 0);
    }

    #[test]
    fn suffix_match_scores_match_fzf_cases() {
        assert_score(Algorithm::Suffix, true, b"fooBarbaz", b"Baz", 0);
        assert_score(Algorithm::Suffix, false, b"fooBarbaz", b"foo", 0);
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarbaz",
            b"baz",
            SCORE_MATCH * 3 + BONUS_CONSECUTIVE * 2,
        );
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarBaZ",
            b"baz",
            (SCORE_MATCH + CAMEL_CASE_BONUS) * 3
                + CAMEL_CASE_BONUS * (BONUS_FIRST_CHAR_MULTIPLIER - 1),
        );
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarbaz ",
            b"baz",
            SCORE_MATCH * 3 + BONUS_CONSECUTIVE * 2,
        );
        assert_score(
            Algorithm::Suffix,
            false,
            b"fooBarbaz ",
            b"baz ",
            SCORE_MATCH * 4 + BONUS_CONSECUTIVE * 2 + BOUNDARY_BONUS,
        );
    }

    #[test]
    fn empty_pattern_scores_match_fzf_cases() {
        for algorithm in [
            Algorithm::FuzzyV1,
            Algorithm::FuzzyV2,
            Algorithm::Exact,
            Algorithm::Prefix,
            Algorithm::Suffix,
        ] {
            assert_score(algorithm, true, b"foobar", b"", 0);
        }
    }

    #[test]
    fn long_string_match_past_u16_max_matches_fzf_case() {
        let mut text = vec![b'x'; u16::MAX as usize * 2];
        text.insert(u16::MAX as usize, b'z');

        let result = run_match(Algorithm::FuzzyV2, true, &text, b"zx");

        assert_eq!(result.start, u16::MAX as isize);
        assert_eq!(result.end, u16::MAX as isize + 2);
        assert_eq!(result.score, SCORE_MATCH * 2 + BONUS_CONSECUTIVE);
    }
}

}
